//! The limits, and who may change them.
//!
//! `faucet_funds_alice` proves the faucet pays. This proves it stops, and
//! that only one key can talk it out of stopping.
//!
//! Three things that unit tests cannot show, because they are about the
//! wire and the process rather than about arithmetic:
//!
//! - the **global cap** refuses a key that is inside its own quota, which is
//!   the control that actually bounds a script, since new keys are free;
//! - a **Lightning URI** is refused for a stated reason rather than
//!   attempted, matching what `get_info` advertises;
//! - the **control key** can pause and change policy live, and no other key
//!   can — including one with a perfectly good wallet connection.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use nwc::nostr::nips::nip04;
use nwc::nostr::nips::nip47::{
    Method, PayBip321Request, Request, RequestParams, Response,
};
use serde_json::{json, Value};

use dln_e2e_harness::bitcoind::BitcoindHarness;
use dln_e2e_harness::process::ManagedChild;
use dln_e2e_harness::{relay, util};

const PREMINE: u64 = 110;
const ONE_COIN_SAT: u64 = 100_000_000;
const WINDOW_SECS: u64 = 120;
/// Two coins across everyone, so a third asker hits it while still inside
/// their own untouched one-coin quota.
const TOTAL_CAP_SAT: u64 = 200_000_000;

const CONTROL_REQUEST_KIND: u16 = 23198;
const CONTROL_RESPONSE_KIND: u16 = 23199;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    match run().await {
        Ok(()) => {
            println!("\n=== PASS ===");
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("\n=== FAIL ===\n{e:?}");
            std::process::exit(1);
        }
    }
}

async fn run() -> Result<()> {
    let out = PathBuf::from(
        std::env::var("OUTPUT_DIR").unwrap_or_else(|_| util::unique_tmp_dir("faucet-limits")),
    );
    std::fs::create_dir_all(&out)?;

    let faucet_bin = faucet_binary()?;
    let (_relay_container, relay_url) = relay::start_relay().await;

    let node = BitcoindHarness::start().await;
    let miner_addr = node.get_new_address().await;
    node.mine_blocks(PREMINE, &miner_addr).await;
    node.create_wallet("treasury").await;

    let control = Keys::generate();
    let (faucet_pubkey, _child) =
        start_faucet(&node, &relay_url, &faucet_bin, &out, &control.public_key()).await?;

    // ── The global cap ───────────────────────────────────────────────────
    // Two keys take a coin each, exhausting a two-coin cap. A third key has
    // taken nothing and is well inside its own quota, and must still be
    // refused — that is the whole point of having a cap as well as a quota.
    tracing::info!("Step 1: two keys exhaust the global cap");
    for i in 1..=2 {
        let asker = Keys::generate();
        let addr = treasury_address(&node).await?;
        let resp = ask(&relay_url, &asker, &faucet_pubkey, &format!("bitcoin:{addr}?amount=1")).await?;
        anyhow::ensure!(resp.error.is_none(), "asker {i} was refused: {:?}", resp.error);
        node.mine_blocks(1, &miner_addr).await;
    }

    tracing::info!("Step 2: a third key, inside its own quota, is refused by the cap");
    let fresh = Keys::generate();
    let addr = treasury_address(&node).await?;
    let resp = ask(&relay_url, &fresh, &faucet_pubkey, &format!("bitcoin:{addr}?amount=1")).await?;
    let err = resp
        .error
        .context("a key inside its own quota was paid past the global cap — the cap is not enforced")?;
    anyhow::ensure!(
        err.message.contains("capped"),
        "refused, but not by the cap: {}",
        err.message
    );
    anyhow::ensure!(
        err.message.contains("not about your key"),
        "the refusal blames the asker for a faucet-wide limit: {}",
        err.message
    );
    tracing::info!("  refused — {}", err.message);

    // ── A Lightning URI, which this faucet cannot pay ────────────────────
    tracing::info!("Step 3: a lightning-only URI is refused for a stated reason");
    let lnonly = Keys::generate();
    let resp = ask(
        &relay_url,
        &lnonly,
        &faucet_pubkey,
        "bitcoin:?lightning=lnbc1p000000000000000000000000000000000000000000",
    )
    .await?;
    let err = resp.error.context("a lightning URI was accepted by an on-chain-only faucet")?;
    anyhow::ensure!(
        err.message.contains("on-chain only"),
        "refused, but without saying why: {}",
        err.message
    );
    tracing::info!("  refused — {}", err.message);

    // ── The control key ──────────────────────────────────────────────────
    tracing::info!("Step 4: a key that is not the control key cannot control it");
    let impostor = Keys::generate();
    let resp = control_call(&relay_url, &impostor, &faucet_pubkey, json!({"method": "pause"})).await?;
    anyhow::ensure!(
        resp["ok"] == json!(false),
        "an arbitrary key was allowed to pause the faucet: {resp}"
    );
    tracing::info!("  refused — {}", resp["message"].as_str().unwrap_or(""));

    tracing::info!("Step 5: the control key raises the cap, live, with no restart");
    let resp = control_call(
        &relay_url,
        &control,
        &faucet_pubkey,
        json!({"method": "set_policy", "params": {"total_cap_sat": 1_000_000_000u64}}),
    )
    .await?;
    anyhow::ensure!(resp["ok"] == json!(true), "control key was refused: {resp}");
    tracing::info!("  {}", resp["message"].as_str().unwrap_or(""));

    // The same key that was capped a moment ago should now be paid, and the
    // process was never restarted.
    let addr = treasury_address(&node).await?;
    let resp = ask(&relay_url, &fresh, &faucet_pubkey, &format!("bitcoin:{addr}?amount=1")).await?;
    anyhow::ensure!(
        resp.error.is_none(),
        "raising the cap did not take effect without a restart: {:?}",
        resp.error
    );
    tracing::info!("  the previously-capped key is now paid");

    tracing::info!("Step 6: the control key pauses, and everyone is refused");
    let resp = control_call(&relay_url, &control, &faucet_pubkey, json!({"method": "pause"})).await?;
    anyhow::ensure!(resp["ok"] == json!(true), "pause was refused: {resp}");

    let addr = treasury_address(&node).await?;
    let resp = ask(&relay_url, &Keys::generate(), &faucet_pubkey, &format!("bitcoin:{addr}?amount=1")).await?;
    let err = resp.error.context("the faucet paid while paused")?;
    anyhow::ensure!(err.message.contains("paused"), "refused, but not as paused: {}", err.message);
    tracing::info!("  refused — {}", err.message);

    Ok(())
}

// ── plumbing ─────────────────────────────────────────────────────────────

fn faucet_binary() -> Result<String> {
    if let Ok(p) = std::env::var("NOSTR_FAUCET_BINARY") {
        return Ok(p);
    }
    let repo = std::env::var("NOSTR_FAUCET_REPO")
        .unwrap_or_else(|_| format!("{}/git/nostr-faucet", std::env::var("HOME").unwrap()));
    let status = std::process::Command::new("cargo")
        .args(["build", "--bin", "nostr-faucet"])
        .current_dir(&repo)
        .status()?;
    anyhow::ensure!(status.success(), "building nostr-faucet failed");
    Ok(format!("{repo}/target/debug/nostr-faucet"))
}

async fn start_faucet(
    node: &BitcoindHarness,
    relay_url: &str,
    binary: &str,
    out: &std::path::Path,
    control_pubkey: &PublicKey,
) -> Result<(PublicKey, ManagedChild)> {
    let keys = Keys::generate();
    std::fs::create_dir_all(out)?;
    let cfg_path = out.join("faucet.toml");
    std::fs::write(
        &cfg_path,
        format!(
            r#"
[nostr]
relay = "{relay_url}"
secret_key = "{}"
control_pubkey = "{}"

[bitcoind]
rpc_host = "127.0.0.1"
rpc_port = {}
rpc_user = "{}"
rpc_password = "{}"
wallet = "testwallet"
chain_label = "limits.regtest"

[policy]
per_key_sat = {ONE_COIN_SAT}
window_secs = {WINDOW_SECS}
total_cap_sat = {TOTAL_CAP_SAT}
max_requests_per_window = 10
paused = false
"#,
            keys.secret_key().to_secret_hex(),
            control_pubkey.to_hex(),
            node.rpc_port(),
            node.rpc_user(),
            node.rpc_password(),
        ),
    )?;
    let child = ManagedChild::spawn("faucet", binary, &[cfg_path.to_str().unwrap()], out)?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    Ok((keys.public_key(), child))
}

async fn ask(relay: &str, asker: &Keys, faucet: &PublicKey, uri: &str) -> Result<Response> {
    let req = Request {
        method: Method::PayBip321,
        params: RequestParams::PayBip321(PayBip321Request { uri: uri.to_string() }),
    };
    let raw = round_trip(
        relay,
        asker,
        faucet,
        Kind::WalletConnectRequest,
        Kind::WalletConnectResponse,
        serde_json::to_string(&req)?,
    )
    .await?;
    Ok(serde_json::from_str(&raw)?)
}

async fn control_call(
    relay: &str,
    sender: &Keys,
    faucet: &PublicKey,
    body: Value,
) -> Result<Value> {
    let raw = round_trip(
        relay,
        sender,
        faucet,
        Kind::Custom(CONTROL_REQUEST_KIND),
        Kind::Custom(CONTROL_RESPONSE_KIND),
        body.to_string(),
    )
    .await?;
    Ok(serde_json::from_str(&raw)?)
}

async fn round_trip(
    relay: &str,
    sender: &Keys,
    faucet: &PublicKey,
    req_kind: Kind,
    resp_kind: Kind,
    plaintext: String,
) -> Result<String> {
    let client = Client::builder().signer(sender.clone()).build();
    client.add_relay(relay).await?;
    client.connect().await;

    client
        .subscribe(
            Filter::new()
                .kind(resp_kind)
                .pubkey(sender.public_key())
                .since(Timestamp::now()),
        )
        .await?;

    let encrypted = nip04::encrypt(sender.secret_key(), faucet, plaintext)?;
    client
        .send_event_builder(EventBuilder::new(req_kind, encrypted).tag(Tag::public_key(*faucet)))
        .await?;

    let mut notifications = client.notifications();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        anyhow::ensure!(!remaining.is_zero(), "no answer from the faucet in 30s");
        let next = tokio::time::timeout(remaining, notifications.next()).await;
        let Ok(Some(ClientNotification::Event { event, .. })) = next else { continue };
        if event.kind != resp_kind || event.pubkey != *faucet {
            continue;
        }
        let out = nip04::decrypt(sender.secret_key(), faucet, &event.content)?;
        let _ = client.disconnect().await;
        return Ok(out);
    }
}

async fn treasury_address(node: &BitcoindHarness) -> Result<String> {
    node.rpc_wallet("treasury", "getnewaddress", json!(["", "bech32"]))
        .await
        .map_err(|e| anyhow::anyhow!("treasury getnewaddress: {e}"))?
        .as_str()
        .map(String::from)
        .context("getnewaddress did not return a string")
}
